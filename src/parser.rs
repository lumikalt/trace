//! Hand-written recursive descent. Items and statements are plain RD;
//! expressions go through a Pratt loop so each operator tier is one
//! binding-power entry. Statements end at newlines, `;`, or `}`.
//!
//! Recovery: a statement- or item-level error skips to the next newline
//! (or closing brace) and parsing continues, so one typo reports once.

use crate::ast::{
    Ast, BinOp, Destructure, Effect, Expr, ExprId, ExtPort, ExtPortDir, FnKind, Item, ItemId, Name,
    Param, ScheduleDirective, Stmt, StmtId, UnOp,
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

/// `parse_struct_lit_fields`'s own return shape: the explicit `field:
/// expr` list, plus a trailing `..base` if present.
type StructLitFields = (Vec<(String, ExprId)>, Option<ExprId>);

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
        DotDot | PlusColon | MinusColon => (1, 2),
        Eq | LtGt | Lt | Le | Gt | Ge => (3, 4),
        Pipe => (5, 6),
        Caret => (7, 8),
        Amp => (9, 10),
        Shl | Shr | AShr => (11, 12),
        Plus | Minus => (13, 14),
        Star | Slash | Percent => (15, 16),
        _ => return None,
    };
    Some(bp)
}

const PREFIX_BP: u8 = 17;
const POSTFIX_BP: u8 = 19;

/// Minimum binding power for type positions (`: ty`). Excludes comparison
/// and range operators so the `<` of a following effect list (`: [1]
/// <combines>`) is never eaten as less-than. Bracket application and
/// arithmetic (`[N+1]`) still parse.
const TYPE_MIN_BP: u8 = 5;

fn binop_of(kind: TokenKind) -> BinOp {
    use TokenKind::*;
    match kind {
        DotDot => BinOp::Range,
        PlusColon => BinOp::PlusColon,
        MinusColon => BinOp::MinusColon,
        Eq => BinOp::Eq,
        LtGt => BinOp::Ne,
        Lt => BinOp::Lt,
        Le => BinOp::Le,
        Gt => BinOp::Gt,
        Ge => BinOp::Ge,
        Pipe => BinOp::BitOr,
        Caret => BinOp::BitXor,
        Amp => BinOp::BitAnd,
        Shl => BinOp::Shl,
        Shr => BinOp::Shr,
        AShr => BinOp::AShr,
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

    fn peek_nth(&self, n: usize) -> Option<TokenKind> {
        self.tokens.get(self.pos + n).map(|t| t.kind)
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
            let items = self.parse_item();
            self.ast.roots.extend(items);
            // Recovery must always make progress: `sync` stops before `}`,
            // which nothing consumes at top level. Never loop on one token.
            if self.pos == before {
                self.bump();
            }
        }
    }

    /// Returns every item one `parse_item` call produced — almost always
    /// zero or one, except `rule foo?` (see `parse_rule`), which splices
    /// several synthesized items in at once, the same "real AST nodes,
    /// spliced at parse time" move `eat_leading_tick` already uses at
    /// statement level.
    fn parse_item(&mut self) -> Vec<ItemId> {
        use TokenKind::*;
        match self.peek() {
            Some(Module) => self.parse_module().into_iter().collect(),
            Some(ExtModule) => self.parse_extmodule().into_iter().collect(),
            Some(Struct) => self.parse_struct().into_iter().collect(),
            Some(Reg) => self.parse_state_decl(Reg).into_iter().collect(),
            Some(Mem) => self.parse_state_decl(Mem).into_iter().collect(),
            Some(Fifo) => self.parse_state_decl(Fifo).into_iter().collect(),
            Some(Input) => self.parse_state_decl(Input).into_iter().collect(),
            Some(Output) => self.parse_state_decl(Output).into_iter().collect(),
            Some(Io) => self.parse_state_decl(Io).into_iter().collect(),
            Some(Attach) => self.parse_attach().into_iter().collect(),
            Some(Inst) => self.parse_state_decl(Inst).into_iter().collect(),
            Some(Rule) => self.parse_rule(),
            Some(Ident) => self.parse_fn(FnFlavor::Fn).into_iter().collect(),
            Some(Spec) => {
                self.bump();
                self.parse_fn(FnFlavor::Spec).into_iter().collect()
            }
            Some(Impl) => {
                self.bump();
                self.parse_fn(FnFlavor::Impl).into_iter().collect()
            }
            Some(Schedule) => self.parse_schedule().into_iter().collect(),
            _ => {
                self.error_here(
                    "expected an item (module, extmodule, struct, reg, mem, fifo, in, out, \
                     io, attach, rule, schedule, or a function)"
                        .to_string(),
                );
                self.sync();
                Vec::new()
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
                    items.extend(self.parse_item());
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

    /// `struct Name { field : ty \n ... }` — newline-terminated field
    /// list, same brace-block shape `parse_module` uses (not comma-
    /// separated like `parse_fn`'s parameter list, since a field list
    /// reads more naturally one per line matching every other multi-
    /// line body in this language).
    fn parse_struct(&mut self) -> Option<ItemId> {
        let lo = self.cur_span().start;
        self.bump(); // struct
        let name = self.expect_ident("struct name")?;
        self.expect(TokenKind::LBrace, "`{` after struct name")
            .ok()?;
        let mut fields = Vec::new();
        loop {
            self.skip_newlines();
            match self.peek() {
                Some(TokenKind::RBrace) => {
                    self.bump();
                    break;
                }
                None => {
                    self.error_here("unclosed struct body".to_string());
                    break;
                }
                _ => {
                    let fname = self.expect_ident("field name")?;
                    self.expect(TokenKind::Colon, "`:` before field type")
                        .ok()?;
                    let ty = self.parse_expr(TYPE_MIN_BP)?;
                    fields.push(Param {
                        name: fname,
                        ty,
                        bound: None,
                        lower: None,
                    });
                    self.expect_terminator();
                }
            }
        }
        Some(
            self.ast
                .push_item(Item::Struct { name, fields }, lo..self.prev_end),
        )
    }

    /// `extmodule Name from "path.v" { in/out/io port : ty \n ... }`.
    /// `from` is a contextual word (matched by text), not a reserved
    /// keyword — see `TokenKind::ExtModule`'s own doc comment.
    fn parse_extmodule(&mut self) -> Option<ItemId> {
        let lo = self.cur_span().start;
        self.bump(); // extmodule
        let name = self.expect_ident("extmodule name")?;
        let from = self.expect_ident("`from`")?;
        if from.text != "from" {
            self.errors.push(ParseError {
                span: from.span,
                message: format!("expected `from`, found `{}`", from.text),
            });
            return None;
        }
        let path_span = self.expect(TokenKind::Str, "a quoted `.v` path").ok()?;
        let path_text = self.text(&path_span);
        let path = path_text[1..path_text.len() - 1].to_string();
        self.expect(TokenKind::LBrace, "`{` after extmodule path")
            .ok()?;
        let mut ports = Vec::new();
        loop {
            self.skip_newlines();
            let dir = match self.peek() {
                Some(TokenKind::RBrace) => {
                    self.bump();
                    break;
                }
                None => {
                    self.error_here("unclosed extmodule body".to_string());
                    break;
                }
                Some(TokenKind::Input) => ExtPortDir::In,
                Some(TokenKind::Output) => ExtPortDir::Out,
                Some(TokenKind::Io) => ExtPortDir::Io,
                _ => {
                    self.error_here("expected `in`, `out`, or `io`".to_string());
                    self.sync();
                    continue;
                }
            };
            self.bump(); // in/out/io
            let pname = self.expect_ident("port name")?;
            self.expect(TokenKind::Colon, "`:` before port type").ok()?;
            let ty = self.parse_expr(TYPE_MIN_BP)?;
            ports.push(ExtPort {
                dir,
                name: pname,
                ty,
            });
            self.expect_terminator();
        }
        Some(
            self.ast
                .push_item(Item::ExtModule { name, path, ports }, lo..self.prev_end),
        )
    }

    /// Parses an optional `where <ident> < <const>` or `where <const>
    /// <= <ident> < <const>` clause, returning `(bound, lower)` (`(None,
    /// None)` if no `where` is present at all). Shared by
    /// `parse_state_decl` (reg/out, which additionally restricts WHERE
    /// the result may be non-`None`) and `parse_fn`'s param loop (v12,
    /// unconditionally allowed on any param). Built manually rather
    /// than via a single `self.parse_expr(0)` call on the whole clause:
    /// `=` is ALSO a comparison operator (`BinOp::Eq`) at the identical
    /// binding-power tier as `<`/`<=`, so a full low-bp parse would
    /// greedily chain straight into a following `= init` as `(i < 10) =
    /// 0` instead of stopping at `10` — and a chained `L <= i < K`
    /// would itself parse as `(L <= i) < K` under the general grammar.
    /// Parsing every operand separately at `TYPE_MIN_BP` (above the
    /// comparison tier, so no operand ever tries to consume a
    /// `<`/`<=`/`=` itself) sidesteps both ambiguities.
    fn parse_where_bound(&mut self) -> Option<(Option<ExprId>, Option<ExprId>)> {
        if !self.at_ident_text("where") {
            return Some((None, None));
        }
        self.bump();
        let bound_lo = self.cur_span().start;
        let first = self.parse_expr(TYPE_MIN_BP)?;
        let (lower, ident) = if self.eat(TokenKind::Le) {
            let ident = self.parse_expr(TYPE_MIN_BP)?;
            (Some(first), ident)
        } else {
            (None, first)
        };
        self.expect(TokenKind::Lt, "`<` after `where <ident>`")
            .ok()?;
        let rhs = self.parse_expr(TYPE_MIN_BP)?;
        let bound = self.ast.push_expr(
            Expr::Binary {
                op: BinOp::Lt,
                lhs: ident,
                rhs,
            },
            bound_lo..self.prev_end,
        );
        Some((Some(bound), lower))
    }

    /// `reg name : ty (= init)?` / `mem name : ty` / `fifo name : ty` /
    /// `in name : ty` / `out name : ty (= init)?` / `io name : ty` /
    /// `inst name : Module`
    ///
    /// `reg`/`out` may omit `: ty` when initialized with a sized literal
    /// (`reg a = 8'd6`) — the literal's own width becomes the declared
    /// type, synthesized as the same `bits[width]` expression the explicit
    /// syntax would parse to, so nothing downstream of the parser needs to
    /// know the type was inferred rather than written. No other
    /// initializer shape can stand in for an explicit type: a sized
    /// literal is the only expression with a definite width before any
    /// type-checking runs (see types.rs's two literal-typing models).
    fn parse_state_decl(&mut self, keyword: TokenKind) -> Option<ItemId> {
        let lo = self.cur_span().start;
        self.bump(); // keyword
        let name = self.expect_ident("declaration name")?;
        let infers_ty = matches!(keyword, TokenKind::Reg | TokenKind::Output);
        let ty = if self.eat(TokenKind::Colon) {
            if keyword == TokenKind::Fifo && self.at(TokenKind::LBrace) {
                Some(self.parse_fifo_depth_ty()?)
            } else {
                Some(self.parse_expr(TYPE_MIN_BP)?)
            }
        } else {
            if !infers_ty {
                self.expect(TokenKind::Colon, "`:` before type").ok()?;
            }
            None
        };
        // v0 restriction: `reg`/`out`/`mem` only (an `in` has no write
        // site at all to prove anything over, see DESIGN.md; `out` is
        // register-backed and written via the same `Stmt::Assign` shape
        // a `reg` is, so the identical induction argument applies
        // unchanged; `mem`'s own bound is checked the same way at every
        // `m[...] := ...` write site instead, v17). Parsed BEFORE
        // `= init` (`resolve.rs`/`bounds.rs` validate the actual shape,
        // same precedent as `IfLet`'s `init`) — see `parse_where_bound`'s
        // own doc comment for why it's hand-rolled rather than a single
        // `parse_expr(0)` call.
        let where_span = self.cur_span();
        let (bound, lower) = self.parse_where_bound()?;
        if bound.is_some()
            && keyword != TokenKind::Reg
            && keyword != TokenKind::Output
            && keyword != TokenKind::Mem
        {
            self.errors.push(ParseError {
                span: where_span.clone(),
                message: "`where` is only allowed on `reg`/`out`/`mem` declarations (v0 \
                          restriction)"
                    .to_string(),
            });
        }
        let item = match keyword {
            TokenKind::Reg | TokenKind::Output => {
                if ty.is_none() && !self.at(TokenKind::Eq) {
                    self.errors.push(ParseError {
                        span: self.cur_span(),
                        message: "expected `:` before type, or `=` with a sized-literal \
                                  initializer to infer it"
                            .to_string(),
                    });
                    return None;
                }
                if bound.is_some() && !self.at(TokenKind::Eq) {
                    self.errors.push(ParseError {
                        span: where_span,
                        message: "a `where` bound requires an explicit `= init` value (v0 \
                                  restriction) -- the declared bound needs a verified base case"
                            .to_string(),
                    });
                    return None;
                }
                let init = if self.eat(TokenKind::Eq) {
                    Some(self.parse_expr(0)?)
                } else {
                    None
                };
                let ty = match ty {
                    Some(ty) => ty,
                    None => self.infer_ty_from_sized_literal(init.unwrap())?,
                };
                if keyword == TokenKind::Reg {
                    Item::Reg {
                        name,
                        ty,
                        init,
                        bound,
                        lower,
                    }
                } else {
                    Item::Output {
                        name,
                        ty,
                        init,
                        bound,
                        lower,
                    }
                }
            }
            TokenKind::Mem => Item::Mem {
                name,
                ty: ty.unwrap(),
                bound,
                lower,
            },
            TokenKind::Fifo => Item::Fifo {
                name,
                ty: ty.unwrap(),
            },
            TokenKind::Input => Item::Input {
                name,
                ty: ty.unwrap(),
            },
            TokenKind::Io => Item::Io {
                name,
                ty: ty.unwrap(),
            },
            TokenKind::Inst => Item::Inst {
                name,
                module: ty.unwrap(),
            },
            _ => unreachable!(),
        };
        self.expect_terminator();
        Some(self.ast.push_item(item, lo..self.prev_end))
    }

    /// `attach a, b` — each operand parsed as a general expression, same
    /// as `Stmt::Assign`'s `lhs` (see that arm in `parse_stmt`): resolve.rs
    /// and types.rs are what actually restrict the shape to a bare `io`
    /// port name or `instance.port`, not the parser.
    fn parse_attach(&mut self) -> Option<ItemId> {
        let lo = self.cur_span().start;
        self.bump(); // attach
        let a = self.parse_expr(0)?;
        self.expect(TokenKind::Comma, "`,` between attach operands")
            .ok()?;
        let b = self.parse_expr(0)?;
        self.expect_terminator();
        Some(self.ast.push_item(Item::Attach { a, b }, lo..self.prev_end))
    }

    /// `{depth}elem_ty` — a fifo's depth, written before its element
    /// type (Lumi's pick over mem's postfix `elem[len]` spelling — a
    /// fifo's depth reads more naturally up front). Curly braces, not
    /// square brackets: a bare leading `[` in a fifo's own type position
    /// is the ordinary `[N]`/list-literal primary (see `parse_expr`'s
    /// `Some(LBracket)` arm) — `fifo f : {16}[8]` needs its OWN bracket
    /// shape to stay unambiguous from that, distinct from `[16][8]`,
    /// which would otherwise misparse as a nested-list type entirely. A
    /// bare leading `{` is otherwise unused in a type position (a struct
    /// literal always follows a type NAME, never opens one cold), so
    /// this can't collide with anything the general Pratt parser already
    /// handles; only reachable here, from a `fifo` declaration's own
    /// type position. Parses to the exact same
    /// `Bracket { callee: elem_ty, args: [depth] }` shape a postfix
    /// `elem_ty[depth]` would produce, so types.rs's existing elem/len
    /// extraction (written for `mem`) is reused as-is rather than adding
    /// a second copy of that logic.
    fn parse_fifo_depth_ty(&mut self) -> Option<ExprId> {
        let lo = self.cur_span().start;
        self.bump(); // `{`
        let depth = self.parse_expr(0)?;
        self.expect(TokenKind::RBrace, "`}` after fifo depth")
            .ok()?;
        let elem = self.parse_expr(TYPE_MIN_BP)?;
        Some(self.ast.push_expr(
            Expr::Bracket {
                callee: elem,
                args: vec![depth],
            },
            lo..self.prev_end,
        ))
    }

    /// Synthesizes the `bits[width]` type expression from a literal width
    /// known at parse time — used only by `infer_ty_from_sized_literal`
    /// (below), inferring a `reg`/`out` declaration's type from its own
    /// sized-literal initializer. Delegates to `synth_bits_ty_expr` for
    /// the actual AST shape.
    fn synth_bits_ty(&mut self, width: u64, span: Span) -> ExprId {
        let width_expr = self.ast.push_expr(Expr::Int(width), span.clone());
        self.synth_bits_ty_expr(width_expr, span)
    }

    /// Synthesizes the `bits[width]` type expression by hand, from an
    /// already-parsed width expression — the AST shape a literal
    /// `bits[width]` used to parse to directly
    /// (`Bracket { callee: Ident("bits"), args: [width] }`), so nothing
    /// downstream (resolve/effects/types/emission) can tell the
    /// difference between this and the retired explicit spelling. Used by
    /// `parse_expr`'s `[N]` primary arm (the ordinary case) and its
    /// `bits[N]`-rejection recovery path (the retired-spelling case).
    fn synth_bits_ty_expr(&mut self, width: ExprId, span: Span) -> ExprId {
        let bits_ident = self
            .ast
            .push_expr(Expr::Ident("bits".to_string()), span.clone());
        self.ast.push_expr(
            Expr::Bracket {
                callee: bits_ident,
                args: vec![width],
            },
            span,
        )
    }

    /// Synthesizes the `bits[width]` type expression a sized literal's own
    /// width implies, for a `reg`/`out` declaration that omitted `: ty`.
    /// Errors if the initializer isn't literally a sized literal — a bare
    /// `Int` has no width of its own (it absorbs one from context, which
    /// doesn't exist yet at this declaration), and a larger expression's
    /// width isn't knowable this early in the pipeline.
    fn infer_ty_from_sized_literal(&mut self, init: ExprId) -> Option<ExprId> {
        let span = self.ast.expr_spans[init.0 as usize].clone();
        let Expr::SizedInt { width, .. } = self.ast.expr(init) else {
            self.errors.push(ParseError {
                span: span.clone(),
                message: "cannot infer a type here: the initializer must be a sized \
                          literal like `8'd6`, or give an explicit `: [N]`"
                    .to_string(),
            });
            return None;
        };
        let width = *width;
        Some(self.synth_bits_ty(width, span))
    }

    /// `rule name <effects>? { body }`, or, with a `?` directly after the
    /// name (before any `<effects>` tag list, mirroring `?T`'s own use as
    /// a type marker): `rule foo? { body }`, sugar for an implicit,
    /// rising-edge-triggered enable port sharing the rule's own name (see
    /// TODO.md's "Rules: optional/enable sugar" — Lumi's call, via
    /// `AskUserQuestion`). Desugars to five items, spliced in as real AST
    /// nodes at parse time (see `desugar_optional_rule`).
    fn parse_rule(&mut self) -> Vec<ItemId> {
        let lo = self.cur_span().start;
        self.bump(); // rule
        let Some(name) = self.expect_ident("rule name") else {
            return Vec::new();
        };
        let sugar = self.eat(TokenKind::Question);
        let Some(effects) = self.parse_effects() else {
            return Vec::new();
        };
        self.skip_newlines();
        let Some(body) = self.parse_block() else {
            return Vec::new();
        };
        if sugar && let Some(seq) = effects.iter().find(|e| e.name.text == "sequences") {
            // `lower.rs` splices SOURCE TEXT by span, reconstructing a
            // `<sequences>` rule's segments from the original file's own
            // byte ranges (see DESIGN.md's "Optional rule sugar" for why
            // this sugar's own splice, unlike that one, is safe to do
            // directly on the AST instead). Every node this desugaring
            // synthesizes shares `name`'s span — fine for everything
            // downstream, which reads structure, not source text, but
            // `lower.rs` computes segment-boundary edits FROM spans, and
            // a synthesized node's span colliding with the real `foo`
            // token's own span produces overlapping edits, a hard panic
            // (`lower.rs`'s own "overlapping lowering edits" assertion,
            // confirmed by hand before writing this check, not guessed).
            // A v0 restriction, not a permanent one: the manual `in foo :
            // [1]` + `foo?` pattern (see the `spawn`/`race` examples)
            // still works fine under `<sequences>`.
            self.errors.push(ParseError {
                span: seq.name.span.clone(),
                message: "`rule foo? <sequences>` isn't supported yet: write the \
                          enable check by hand instead (`in foo : [1]` plus `foo?` \
                          as the rule's first statement)"
                    .to_string(),
            });
            return Vec::new();
        }
        if !sugar {
            return vec![self.ast.push_item(
                Item::Rule {
                    name,
                    effects,
                    body,
                },
                lo..self.prev_end,
            )];
        }
        self.desugar_optional_rule(name, effects, body, lo)
    }

    /// `rule foo? <effects> { body }` --> five items, all sharing `foo`'s
    /// own span (they're synthesized, not really written anywhere, but a
    /// span pointing at `foo` itself is far more useful in an error
    /// message than the empty span an entirely fabricated one would give):
    ///
    /// ```text
    /// in foo : [1]
    /// reg __prev_foo : [1] = 1
    /// rule __edge_foo {
    ///     __prev_foo := foo
    /// }
    /// rule foo <effects> {
    ///     (foo & not(__prev_foo))?
    ///     body...
    /// }
    /// schedule {
    ///     conflict_free { __edge_foo, foo }
    /// }
    /// ```
    ///
    /// `__edge_foo` has to be a second, always-firing rule rather than
    /// folded into `foo` itself: the shadow register's update must happen
    /// EVERY cycle regardless of whether `foo` fires, or the edge history
    /// it tracks would only advance on cycles `foo` fires, corrupting the
    /// very detection it exists to support. That makes it an ordinary
    /// read/write conflict against `foo` in schedule.rs's eyes (both touch
    /// `__prev_foo`), hence the synthesized `conflict_free` exemption —
    /// sound here specifically because `foo`'s guard reads `__prev_foo`'s
    /// pre-this-cycle value regardless of any same-cycle write to it, same
    /// as any other register read.
    ///
    /// `__prev_foo` resets to `1`, not `0` — the one real semantic choice
    /// here, not just a naming detail. It's what makes a port already held
    /// high AT reset read as "no edge" (`not(__prev_foo)` is `0` on the
    /// very first post-reset cycle no matter what `foo` itself reads as)
    /// rather than a spurious first-cycle fire, which resetting to `0`
    /// instead would produce (Lumi's call, via `AskUserQuestion`, over the
    /// "reset value doesn't matter, TODO.md's own illustration used `0`"
    /// default).
    fn desugar_optional_rule(
        &mut self,
        name: Name,
        effects: Vec<Effect>,
        body: Vec<StmtId>,
        lo: usize,
    ) -> Vec<ItemId> {
        let span = name.span.clone();
        let prev_name = Name {
            text: format!("__prev_{}", name.text),
            span: span.clone(),
        };
        let edge_name = Name {
            text: format!("__edge_{}", name.text),
            span: span.clone(),
        };

        let port_ty = self.synth_bits_ty(1, span.clone());
        let port = self.ast.push_item(
            Item::Input {
                name: name.clone(),
                ty: port_ty,
            },
            span.clone(),
        );

        let reg_ty = self.synth_bits_ty(1, span.clone());
        let one = self.ast.push_expr(Expr::Int(1), span.clone());
        let shadow = self.ast.push_item(
            Item::Reg {
                name: prev_name.clone(),
                ty: reg_ty,
                init: Some(one),
                bound: None,
                lower: None,
            },
            span.clone(),
        );

        let edge_read_foo = self
            .ast
            .push_expr(Expr::Ident(name.text.clone()), span.clone());
        let edge_write_prev = self
            .ast
            .push_expr(Expr::Ident(prev_name.text.clone()), span.clone());
        let edge_assign = self.ast.push_stmt(
            Stmt::Assign {
                lhs: edge_write_prev,
                rhs: edge_read_foo,
            },
            span.clone(),
        );
        let edge_rule = self.ast.push_item(
            Item::Rule {
                name: edge_name.clone(),
                effects: Vec::new(),
                body: vec![edge_assign],
            },
            span.clone(),
        );

        let guard_foo = self
            .ast
            .push_expr(Expr::Ident(name.text.clone()), span.clone());
        let guard_prev = self
            .ast
            .push_expr(Expr::Ident(prev_name.text.clone()), span.clone());
        let guard_not_prev = self.ast.push_expr(
            Expr::Unary {
                op: UnOp::Not,
                operand: guard_prev,
            },
            span.clone(),
        );
        let guard_cond = self.ast.push_expr(
            Expr::Binary {
                op: BinOp::BitAnd,
                lhs: guard_foo,
                rhs: guard_not_prev,
            },
            span.clone(),
        );
        let guard_expr = self.ast.push_expr(Expr::Guard(guard_cond), span.clone());
        let guard_stmt = self.ast.push_stmt(Stmt::Expr(guard_expr), span.clone());
        let mut new_body = vec![guard_stmt];
        new_body.extend(body);
        let main_rule = self.ast.push_item(
            Item::Rule {
                name: name.clone(),
                effects,
                body: new_body,
            },
            lo..self.prev_end,
        );

        let schedule = self.ast.push_item(
            Item::Schedule {
                directives: vec![ScheduleDirective::ConflictFree(vec![edge_name, name])],
            },
            span,
        );

        vec![port, shadow, edge_rule, main_rule, schedule]
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
            // v12: `where` is unconditionally allowed on any param (no
            // reg/out-only restriction the way `parse_state_decl`'s
            // own call site enforces) -- a bounded param is always
            // meaningful regardless of its own type shape, checked as
            // a call-site obligation by `bounds.rs`.
            let (bound, lower) = self.parse_where_bound()?;
            params.push(Param {
                name: pname,
                ty,
                bound,
                lower,
            });
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
        // v13: `where result < N` on the return type -- checked as a
        // postcondition against every `Stmt::Return` (bounds.rs), then
        // trusted at call sites. No restriction on presence, same as a
        // param's own `where` (unconditional, unlike `parse_state_
        // decl`'s reg/out-only restriction check).
        let (ret_bound, ret_lower) = self.parse_where_bound()?;
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
                ret_bound,
                ret_lower,
                effects,
                body,
            },
            lo..self.prev_end,
        ))
    }

    /// `schedule { urgency a > b \n mutually_exclusive { a, b } \n
    /// conflict_free { c, d } }`
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
        let name = self.expect_ident("`urgency`, `mutually_exclusive`, or `conflict_free`")?;
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
            "mutually_exclusive" => {
                self.expect(TokenKind::LBrace, "`{` after `mutually_exclusive`")
                    .ok()?;
                let mut names = vec![self.expect_ident("rule name")?];
                while self.eat(TokenKind::Comma) {
                    names.push(self.expect_ident("rule name")?);
                }
                self.expect(TokenKind::RBrace, "`}` closing `mutually_exclusive`")
                    .ok()?;
                ScheduleDirective::MutuallyExclusive(names)
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
                    "unknown schedule directive `{name}` (expected `urgency`, \
                     `mutually_exclusive`, or `conflict_free`)"
                ));
                self.sync();
                return None;
            }
        };
        self.expect_terminator();
        Some(directive)
    }

    /// `<name (args)?, ...>` — e.g. `<sequences, reads {pc, mem}>`.
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

    /// Whether the current token is a plain identifier spelled exactly
    /// `text` — used for contextual keywords that only shape ONE
    /// existing construct (`where`, matching this file's own precedent
    /// for `urgency`/`mutually_exclusive`/`conflict_free`: the reserved-
    /// keyword set holds only words that open or shape a construct on
    /// their own, not every word that ever appears in a fixed position).
    fn at_ident_text(&self, text: &str) -> bool {
        self.at(TokenKind::Ident)
            && self
                .tokens
                .get(self.pos)
                .is_some_and(|t| self.text(&t.span) == text)
    }

    /// Name positions accept `reg`/`mem`/`fifo`/`in`/`out`/`io` too: they
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
                | Some(TokenKind::Io)
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
                    if let Some(stmts) = self.parse_stmt() {
                        body.extend(stmts);
                    }
                    if self.pos == before {
                        self.bump();
                    }
                }
            }
        }
        Some(body)
    }

    fn parse_stmt(&mut self) -> Option<Vec<StmtId>> {
        use TokenKind::*;
        let lo = self.cur_span().start;
        let mut extra_guard: Option<StmtId> = None;
        let stmt = match self.peek() {
            Some(Tick) => {
                self.bump();
                match self.peek() {
                    Some(Newline) | Some(Semi) | Some(RBrace) | None => {}
                    // `tick <expr>`: the trailing expression gates entry
                    // into the segment this tick opens — sugar for
                    // writing it as that segment's own first statement
                    // (an explicit `cond?` guard, or an already-fallible
                    // bracket op like `sync[h1, h2]`), so it parses into
                    // an ordinary extra statement rather than a new AST
                    // shape lower.rs would need to know about.
                    _ => {
                        let expr_lo = self.cur_span().start;
                        let expr = self.parse_expr(0)?;
                        extra_guard =
                            Some(self.ast.push_stmt(Stmt::Expr(expr), expr_lo..self.prev_end));
                    }
                }
                self.expect_terminator();
                Stmt::Tick
            }
            Some(Break) => {
                self.bump();
                self.expect_terminator();
                Stmt::Break
            }
            // `let {field, field: bind, ...} = source` — struct/`?T`
            // destructuring, sugar for one `let bind = source.field` per
            // item (`parse_let_destructure`, below). Bare-brace, not
            // `let StructName{...} = source` (Rust's own spelling): the
            // parser has no type information to validate a struct name
            // against `source`'s actual type, and text that READS like an
            // assertion but isn't checked is exactly the kind of sharp
            // edge this codebase avoids elsewhere (see `list[T]`'s own
            // bare-identifier ambiguity, types.rs). `{` here can only
            // mean this — `let` never has a block-shaped RHS otherwise.
            Some(Let) if self.peek_nth(1) == Some(LBrace) => {
                self.bump();
                return self.parse_let_destructure(lo);
            }
            Some(Let) => {
                self.bump();
                let name = self.expect_ident("binding name")?;
                self.expect(Eq, "`=` after `let` name").ok().or_else(|| {
                    self.sync();
                    None
                })?;
                let leading_tick = self.eat_leading_tick();
                let init = self.parse_expr(0)?;
                self.expect_terminator();
                let let_id = self
                    .ast
                    .push_stmt(Stmt::Let { name, init }, lo..self.prev_end);
                let mut out = Vec::new();
                out.extend(leading_tick);
                out.push(let_id);
                return Some(out);
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
            Some(While) => self.parse_while()?,
            _ => {
                let lhs = self.parse_expr(0)?;
                if self.eat(ColonEq) {
                    let leading_tick = self.eat_leading_tick();
                    let rhs = self.parse_expr(0)?;
                    self.expect_terminator();
                    let assign_id = self
                        .ast
                        .push_stmt(Stmt::Assign { lhs, rhs }, lo..self.prev_end);
                    let mut out = Vec::new();
                    out.extend(leading_tick);
                    out.push(assign_id);
                    return Some(out);
                } else {
                    self.expect_terminator();
                    Stmt::Expr(lhs)
                }
            }
        };
        let id = self.ast.push_stmt(stmt, lo..self.prev_end);
        let mut out = vec![id];
        out.extend(extra_guard);
        Some(out)
    }

    /// `let {field, field: bind, ...} = source` — one `let bind =
    /// source.field` per item, in written order (dispatched here with
    /// `{` not yet consumed). `source` is restricted to a bare
    /// identifier (a reg/local/param reference), not a general
    /// expression: a call there would desugar to one re-evaluation per
    /// destructured field (`SomeCall().a`, `SomeCall().b`), silently
    /// duplicating whatever the callee's body does instead of binding
    /// one shared result — sidestepped by requiring the simple case
    /// syntactically rather than chasing which call shapes are actually
    /// safe to duplicate. Each synthetic `Stmt::Let` gets its OWN fresh
    /// `Expr::Ident(source)` (never one shared base `ExprId` reused
    /// across multiple `Expr::Field` parents), so the AST stays a tree —
    /// every existing walker (`sub_exprs`, `collect_calls`, `collect_
    /// fifo_ops`, `infer_expr`) assumes that shape; a shared
    /// subexpression would make it a DAG instead. No nested
    /// destructuring (`{maybe: {valid}}`) and no `..rest` — single-level
    /// field projection only. The struct/`?T`'s actual field names are
    /// validated for free by ordinary `.field` type-checking on each
    /// projection, the same error a hand-written `let bind = source.
    /// field` would already give for a typo — this desugar adds no
    /// validation of its own.
    fn parse_let_destructure(&mut self, lo: usize) -> Option<Vec<StmtId>> {
        self.bump(); // `{`
        self.skip_newlines();
        let mut items: Vec<(Name, Name)> = Vec::new();
        let mut has_rest = false;
        while !self.at(TokenKind::RBrace) {
            // `..`, same trailing-only rule struct update's `..base`
            // has, minus a base identifier — this is a pattern
            // discarding fields, not a value spreading them from
            // somewhere, so there's nothing to name after it.
            if self.eat(TokenKind::DotDot) {
                has_rest = true;
                self.skip_newlines();
                if !self.at(TokenKind::RBrace) {
                    self.error_here(
                        "`..` must be the last item in a destructuring pattern -- no \
                         field may follow it"
                            .to_string(),
                    );
                    self.sync();
                    return None;
                }
                break;
            }
            let field = self.expect_ident("field name")?;
            let bind = if self.eat(TokenKind::Colon) {
                self.expect_ident("binding name")?
            } else {
                field.clone()
            };
            items.push((field, bind));
            self.skip_newlines();
            if !self.eat(TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        self.expect(TokenKind::RBrace, "`}` closing a destructuring pattern")
            .ok()?;
        self.expect(TokenKind::Eq, "`=` after a destructuring pattern")
            .ok()?;
        let source = self.expect_ident(
            "a plain reference to destructure (a reg/local/param name, not a general expression)",
        )?;
        // A dedicated check, not a bare `expect_terminator()`: anything
        // trailing the identifier (`(`, `.`, `[`, ...) means the source
        // wasn't actually a bare reference, and `expect_terminator`'s own
        // generic "expected end of statement" wouldn't say why that
        // matters here.
        match self.peek() {
            Some(TokenKind::Newline) | Some(TokenKind::Semi) => {
                self.bump();
            }
            Some(TokenKind::RBrace) | None => {}
            _ => {
                self.error_here(
                    "a destructuring source must be a plain reference (a reg/local/param \
                     name), not a call/field access/other expression; bind it with an \
                     ordinary `let` first, then destructure that"
                        .to_string(),
                );
                self.sync();
                return None;
            }
        }
        let mut out = Vec::with_capacity(items.len());
        let mut source_field_base = None;
        for (field, bind) in &items {
            let base = self
                .ast
                .push_expr(Expr::Ident(source.text.clone()), source.span.clone());
            source_field_base.get_or_insert(base);
            let value = self.ast.push_expr(
                Expr::Field {
                    base,
                    name: field.text.clone(),
                },
                field.span.clone(),
            );
            let span = field.span.start..bind.span.end;
            out.push(self.ast.push_stmt(
                Stmt::Let {
                    name: bind.clone(),
                    init: value,
                },
                span,
            ));
        }
        // Exhaustiveness (every field of `source`'s type named, or `..`
        // present) needs `source`'s resolved type, which the parser
        // doesn't have — deferred to types.rs (`check_destructures`),
        // which can read it back out of `expr_tys` once `source_field_
        // base` above has been type-checked as an ordinary part of the
        // body walk. Skipped for a zero-item pattern (`let {} = s` /
        // `let {..} = s`): there's no per-item base left to hang the
        // lookup off, and "bind/discard nothing" can't miss a field
        // either way.
        if let Some(source_field_base) = source_field_base {
            self.ast.destructures.push(Destructure {
                span: lo..self.prev_end,
                source_field_base,
                named_fields: items.into_iter().map(|(field, _)| field).collect(),
                has_rest,
            });
        }
        Some(out)
    }

    /// `lhs := tick <expr>` / `let name = tick <expr>`: a leading `tick`
    /// right after `:=`/`=`, consumed and returned as its own statement
    /// if present — sugar for writing the tick on its own line before
    /// the assignment, letting `tick` sit next to the expression it
    /// actually gates (e.g. `value := tick race[f1, f2]`, where the
    /// value only makes sense once `race`'s own guard has succeeded).
    /// Same "ordinary extra statement, no new AST shape" sugar as the
    /// statement-initial `tick <expr>` form above, just recognized in a
    /// different syntactic position.
    fn eat_leading_tick(&mut self) -> Option<StmtId> {
        if !matches!(self.peek(), Some(TokenKind::Tick)) {
            return None;
        }
        let tick_lo = self.cur_span().start;
        self.bump();
        Some(self.ast.push_stmt(Stmt::Tick, tick_lo..self.prev_end))
    }

    fn parse_if(&mut self) -> Option<Stmt> {
        self.bump(); // if
        if self.at(TokenKind::Let) {
            return self.parse_if_let();
        }
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

    /// `if let NAME = EXPR { ... } [else { ... }]` — Option-presence
    /// binding sugar (DESIGN.md's "`if let`: branch-scoped Option-
    /// presence binding"). `if` has already been consumed; `self` is
    /// sitting on `let`. Mirrors `parse_if`'s own block/else/else-if
    /// handling exactly — the only difference is the `let NAME =` prefix,
    /// reusing `Stmt::Let`'s own name/`=` parsing shape. `EXPR` is parsed
    /// as an ordinary expression, no shape restriction here — types.rs is
    /// what requires it be `Expr::Guard(inner)` over `Ty::Option`.
    fn parse_if_let(&mut self) -> Option<Stmt> {
        self.bump(); // let
        let name = self.expect_ident("binding name")?;
        self.expect(TokenKind::Eq, "`=` after `let` name")
            .ok()
            .or_else(|| {
                self.sync();
                None
            })?;
        let init = self.parse_expr(0)?;
        let then_body = self.parse_block()?;
        let else_body = if self.eat(TokenKind::Else) {
            if self.at(TokenKind::If) {
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
        Some(Stmt::IfLet {
            name,
            init,
            then_body,
            else_body,
        })
    }

    fn parse_while(&mut self) -> Option<Stmt> {
        self.bump(); // while
        if self.at(TokenKind::Let) {
            return self.parse_while_let();
        }
        let cond = self.parse_expr(0)?;
        let body = self.parse_block()?;
        self.expect_terminator();
        Some(Stmt::While { cond, body })
    }

    /// `while let NAME = EXPR { ... }` — the loop-shaped sibling of `if
    /// let` (DESIGN.md's "`while`: multi-cycle loops"). `while` has
    /// already been consumed; `self` is sitting on `let`. Mirrors
    /// `parse_if_let`'s `let NAME =` prefix parsing exactly, but with no
    /// `else`/`else if` handling — a loop has nothing to run once
    /// instead of looping.
    fn parse_while_let(&mut self) -> Option<Stmt> {
        self.bump(); // let
        let name = self.expect_ident("binding name")?;
        self.expect(TokenKind::Eq, "`=` after `let` name")
            .ok()
            .or_else(|| {
                self.sync();
                None
            })?;
        let init = self.parse_expr(0)?;
        let body = self.parse_block()?;
        self.expect_terminator();
        Some(Stmt::WhileLet { name, init, body })
    }

    // --- expressions (Pratt) ---

    fn parse_expr(&mut self, min_bp: u8) -> Option<ExprId> {
        use TokenKind::*;
        let lo = self.cur_span().start;

        let mut lhs = match self.peek() {
            Some(Ident) => {
                let span = self.bump().unwrap().span;
                let name = self.text(&span).to_string();
                // `bits[N]` (the old explicit spelling) is a clean,
                // targeted rejection rather than a silent accept: `bits`
                // is a reserved `resolve.rs` `BUILTINS` name with no
                // meaning of its own anymore now that a bare `[N]` covers
                // it (see this file's own `Some(LBracket)` primary arm,
                // below) — so `Ident("bits")` immediately followed by `[`
                // can only ever be someone reaching for the retired
                // spelling, never a legitimate user reference. Recovers by
                // still building the identical `Bracket { Ident("bits"),
                // [N] }` shape the width-expr implies, so one stale
                // `bits[N]` doesn't cascade into unrelated errors below it.
                if name == "bits" && self.at(TokenKind::LBracket) {
                    self.error_here("`bits[N]` is no longer valid syntax; use `[N]`".to_string());
                    self.bump(); // `[`
                    let width = self.parse_expr(0)?;
                    self.expect(TokenKind::RBracket, "`]`").ok()?;
                    self.synth_bits_ty_expr(width, lo..self.prev_end)
                } else {
                    self.ast.push_expr(Expr::Ident(name), span)
                }
            }
            // `sync`/`race` are keywords but appear in call position, and
            // `reg`/`mem`/`fifo`/`in`/`out`/`io` are keywords only at
            // declaration position (`mem[addr]` is an ordinary read). All
            // become plain idents.
            Some(Sync) | Some(Race) | Some(Reg) | Some(Mem) | Some(Fifo) | Some(Input)
            | Some(Output) | Some(Io) => {
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
            Some(SizedInt) => {
                let span = self.bump().unwrap().span;
                let (width, value) = match parse_sized_int(self.text(&span)) {
                    Some(wv) => wv,
                    None => {
                        self.errors.push(ParseError {
                            span: span.clone(),
                            message: "sized literal too large".to_string(),
                        });
                        (1, 0)
                    }
                };
                self.ast.push_expr(Expr::SizedInt { width, value }, span)
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
            // A leading `[` in primary position is one of two things,
            // told apart by content rather than position: `[a, b, c]`
            // (empty, or 2+ comma-separated items) is a `list[T]`
            // literal; `[N]` (exactly one item, no trailing comma) is the
            // `bits[N]` type shorthand instead — the same `Bracket {
            // Ident("bits"), [N] }` shape a literal `bits[N]` used to
            // produce (see `synth_bits_ty_expr`, below), reached here
            // uniformly whether this bracket is a top-level type
            // position or nested inside another bracket's own args
            // (`list[[8]]`, `mem`'s postfix `elem_ty[len]`), since
            // `parse_args` parses every argument through this same
            // primary path. `AdderTree([a, b, c, d])`-style call
            // arguments are unaffected (always 2+ elements in practice);
            // a genuine one-element list literal has no spelling left —
            // no real use of one exists anywhere in this codebase, and a
            // future one would fail with a clear type error (a `bits[N]`
            // type appearing where a value is expected), not a silent
            // miscompile. Postfix `x[...]` (bit-select/fifo-op/mem-index)
            // is unaffected, handled separately below once an `lhs`
            // already exists.
            Some(LBracket) => {
                self.bump();
                self.skip_newlines();
                if self.at(RBracket) {
                    self.bump();
                    self.ast.push_expr(Expr::ListLit(vec![]), lo..self.prev_end)
                } else {
                    let first = self.parse_expr(0)?;
                    self.skip_newlines();
                    if self.eat(Comma) {
                        self.skip_newlines();
                        let mut items = vec![first];
                        while !self.at(RBracket) {
                            items.push(self.parse_expr(0)?);
                            self.skip_newlines();
                            if !self.eat(Comma) {
                                break;
                            }
                            self.skip_newlines();
                        }
                        self.expect(RBracket, "`]`").ok()?;
                        self.ast.push_expr(Expr::ListLit(items), lo..self.prev_end)
                    } else {
                        self.expect(RBracket, "`]`").ok()?;
                        self.synth_bits_ty_expr(first, lo..self.prev_end)
                    }
                }
            }
            // `..hi` — an open-start list slice bound (`xs[..mid]`).
            // Only meaningful as a bracket argument; `type_bracket`
            // (types.rs) rejects it anywhere else. The closing `..`
            // (`hi..`, open-ended) is handled in the infix loop below,
            // since it needs an `lhs` to already exist.
            Some(DotDot) => {
                self.bump();
                let hi = self.parse_expr(2)?;
                self.ast.push_expr(
                    Expr::Range {
                        lo: None,
                        hi: Some(hi),
                    },
                    lo..self.prev_end,
                )
            }
            Some(Minus) => self.parse_prefix(UnOp::Neg)?,
            Some(Not) => self.parse_prefix(UnOp::Not)?,
            Some(Tilde) => self.parse_prefix(UnOp::BitNot)?,
            // `?T` — a prefix use of `?`, unlike the postfix guard `?`
            // (`cond?`, handled in the postfix loop below): the two never
            // collide since a prefix `?` is only reached here, before any
            // operand has been parsed. `types.rs` rejects `Expr::OptionTy`
            // outside an actual type position.
            Some(Question) => {
                self.bump();
                let inner = self.parse_expr(PREFIX_BP)?;
                self.ast.push_expr(Expr::OptionTy(inner), lo..self.prev_end)
            }
            Some(False) => {
                let span = self.bump().unwrap().span;
                self.ast.push_expr(Expr::Absent, span)
            }
            // `optional <expr>` — an explicit one-layer "present"
            // constructor (see `Expr::Optional`'s own doc comment,
            // ast.rs). A real AST node, unlike `?T`'s type-position
            // prefix `?` above: it needs to survive into typing/emission
            // so `??T`'s two `valid` bits can be driven independently.
            Some(Optional) => {
                self.bump();
                let inner = self.parse_expr(PREFIX_BP)?;
                self.ast.push_expr(Expr::Optional(inner), lo..self.prev_end)
            }
            // `logic <expr>` — a prefix operator, not `logic(...)` call
            // syntax (see `Expr::Logic`'s own doc comment, ast.rs).
            // Unlike every OTHER prefix operator here, its operand parses
            // at `0` (a full expression, same as a parenthesized group's
            // inner parse), not `PREFIX_BP` — deliberately loose, so
            // `logic a > b` reads as `logic (a > b)` without parens
            // (Lumi's call: comparisons are `logic`'s main operand shape
            // going forward, see TODO.md's comparisons-as-fallible
            // design). This language's own bitwise operators (`&`/`|`/
            // `^`) bind TIGHTER than comparisons (Rust-style, see
            // `precedence_matches_rust_not_c`), so there is no threshold
            // that swallows a comparison without ALSO swallowing those —
            // `logic A & logic B` (the `and`-combination idiom, DESIGN.md)
            // now needs explicit parens on each side, same as any other
            // expression `&`-combines two things wider than a single
            // token: `(logic A) & (logic B)`.
            Some(Logic) => {
                self.bump();
                let inner = self.parse_expr(0)?;
                self.ast.push_expr(Expr::Logic(inner), lo..self.prev_end)
            }
            _ => {
                self.error_here("expected an expression".to_string());
                self.sync();
                return None;
            }
        };

        while let Some(kind) = self.peek() {
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
                    // `Name { field: expr, ... }` — a struct literal.
                    // Gated on BOTH `lhs` being a bare `Ident` (a struct
                    // type name is never anything else) AND a lookahead
                    // confirming the `{` is followed by `ident :` — not
                    // `ident :=`, and not anything else. That second
                    // condition is what keeps this from misfiring on
                    // `if ready { x := 1 }`: no statement in this
                    // language starts with `ident :`, so the lookahead
                    // never collides with genuine block content, and a
                    // bare-ident condition's `{` is correctly left for
                    // `parse_block` to consume as the if/while body.
                    LBrace
                        if matches!(self.ast.expr(lhs), Expr::Ident(_))
                            && self.at_struct_lit_open() =>
                    {
                        self.bump(); // `{`
                        let (fields, base) = self.parse_struct_lit_fields()?;
                        lhs = self.ast.push_expr(
                            Expr::StructLit {
                                name: lhs,
                                fields,
                                base,
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

            // `lhs..` — an open-end list slice bound (`xs[mid..]`), the
            // trailing counterpart to the leading-`..` case in primary
            // position above. Only recognized when NOTHING that could
            // start an expression follows — a real two-sided range/
            // bit-slice (`x[hi..lo]`) always has a genuine expression
            // right after `..`, so this can never misfire on that,
            // existing, required-both-sides shape.
            if kind == DotDot && infix_bp(kind).is_some_and(|(l_bp, _)| l_bp >= min_bp) {
                let starts_expr = !matches!(
                    self.tokens.get(self.pos + 1).map(|t| t.kind),
                    Some(RBracket) | Some(RParen) | Some(Comma) | None
                );
                if !starts_expr {
                    self.bump();
                    lhs = self.ast.push_expr(
                        Expr::Range {
                            lo: Some(lhs),
                            hi: None,
                        },
                        lo..self.prev_end,
                    );
                    continue;
                }
            }

            // `A or B or C` — Verse's failure-discharging fallback chain,
            // looser-binding than every other operator (`0`, not in
            // `infix_bp`'s table at all) so `a + 1 or b` parses as
            // `(a + 1) or b`, matching every real example. Flattened
            // into one `Expr::Or` at construction time rather than left-
            // nested `Binary`s: `lhs` may already BE an `Or` from a
            // previous iteration of this very loop (`a or b or c` visits
            // this arm twice), in which case the new alternative is
            // appended to its existing list instead of wrapping it in
            // another layer — see ast.rs's `Expr::Or` doc comment for
            // why every downstream consumer wants that flat shape.
            if kind == Or {
                if 0 < min_bp {
                    break;
                }
                self.bump();
                let rhs = self.parse_expr(1)?;
                let mut alts = match self.ast.expr(lhs).clone() {
                    Expr::Or(alts) => alts,
                    _ => vec![lhs],
                };
                alts.push(rhs);
                lhs = self.ast.push_expr(Expr::Or(alts), lo..self.prev_end);
                continue;
            }

            // `A and B and C` — pure sugar for `(logic A) & (logic B) &
            // (logic C)` (see `Expr::Logic`'s doc comment for why that
            // needs explicit parens spelled out by hand): each operand
            // gets its own `Logic` wrap, folded left-to-right with
            // ordinary `&`. Binds at `(2, 3)` — looser than every real
            // operator (comparisons are the loosest at `(3, 4)`, see
            // `infix_bp`) so a whole comparison forms before `and` sees
            // it, but tighter than `or`'s `(0, 1)` so `A or B and C`
            // reads as `A or (B and C)`, matching every real language.
            // The whole chain is consumed in one pass (not flattened
            // across loop iterations like `Or`): `lhs` becomes an
            // ordinary `Binary(BitAnd, ..)` after the first fold, which
            // would be indistinguishable from a user's own literal `&`
            // by shape alone, so re-entering this arm on a later `and`
            // and shape-sniffing `lhs` isn't safe — looping here instead
            // guarantees each operand is wrapped in `Logic` exactly once.
            if kind == And {
                if 2 < min_bp {
                    break;
                }
                let lhs_span = self.ast.expr_spans[lhs.0 as usize].clone();
                self.ast.and_sugar.insert(lhs);
                let mut acc = self.ast.push_expr(Expr::Logic(lhs), lhs_span);
                while self.peek() == Some(And) {
                    self.bump();
                    let rhs = self.parse_expr(3)?;
                    let rhs_span = self.ast.expr_spans[rhs.0 as usize].clone();
                    self.ast.and_sugar.insert(rhs);
                    let rhs_logic = self.ast.push_expr(Expr::Logic(rhs), rhs_span);
                    acc = self.ast.push_expr(
                        Expr::Binary {
                            op: BinOp::BitAnd,
                            lhs: acc,
                            rhs: rhs_logic,
                        },
                        lo..self.prev_end,
                    );
                }
                lhs = acc;
                continue;
            }

            let Some((l_bp, r_bp)) = infix_bp(kind) else {
                break;
            };
            if l_bp < min_bp {
                break;
            }
            self.bump();
            // `a >>.! 300` — an explicit "I know, let it through" on
            // THIS operator application, silencing types.rs's
            // check_literal_fits/check_shift_amount for it specifically
            // (see ast.rs's `lossy` field). Checked right after the
            // operator token itself, before the RHS operand, matching
            // where it's written.
            let lossy = self.eat(TokenKind::Lossy);
            let rhs = self.parse_expr(r_bp)?;
            let bin = self.ast.push_expr(
                Expr::Binary {
                    op: binop_of(kind),
                    lhs,
                    rhs,
                },
                lo..self.prev_end,
            );
            if lossy {
                self.ast.lossy.insert(bin);
            }
            lhs = bin;
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

    /// Whether the CURRENT `{` (not yet consumed) opens a struct
    /// literal: the very next two tokens must be `ident :` — specifically
    /// `Colon`, not `ColonEq` — or a leading `..` (a literal that's
    /// nothing but a spread, `Pair{ ..old }`). No statement in this
    /// language starts with a bare `ident :` or `..`, so neither collides
    /// with genuine block content (see the postfix `LBrace` arm's own
    /// comment for the motivating `if ready { x := 1 }` case). An empty
    /// literal (`Pair{}`) isn't recognized by this lookahead — a v0
    /// non-goal, not a deliberate rejection.
    fn at_struct_lit_open(&self) -> bool {
        // The opening `{` may be followed by a newline before the first
        // field (a multi-line literal, `parse_struct_lit_fields`'s own
        // convention) — skip those before checking shape, same as
        // `if cond {\n x := 1\n }` must still NOT be mistaken for one
        // (the `:` vs `:=` check below is what actually discriminates
        // that case, unaffected by skipping newlines first).
        let mut i = self.pos + 1;
        while matches!(self.tokens.get(i).map(|t| t.kind), Some(TokenKind::Newline)) {
            i += 1;
        }
        matches!(self.tokens.get(i).map(|t| t.kind), Some(TokenKind::DotDot))
            || (matches!(self.tokens.get(i).map(|t| t.kind), Some(TokenKind::Ident))
                && matches!(
                    self.tokens.get(i + 1).map(|t| t.kind),
                    Some(TokenKind::Colon)
                ))
    }

    /// The `{ field: expr, ..., ..base }` tail of a struct literal, with
    /// the opening `{` already consumed — same comma-separated-with-
    /// newlines convention `parse_args` uses, plus a trailing `..base`
    /// (Rust's own spelling: `..` may only be the LAST item, no comma
    /// after it). `base` is required to be a bare identifier — a general
    /// expression would need re-evaluating once per field `..base`
    /// supplies, silently duplicating a call the same way an
    /// unrestricted destructuring source would (see `Expr::StructLit`'s
    /// own doc comment, ast.rs) — enforced here with `expect_ident`
    /// directly rather than parsing a full expression and rejecting its
    /// shape after the fact.
    fn parse_struct_lit_fields(&mut self) -> Option<StructLitFields> {
        let mut fields = Vec::new();
        let mut base = None;
        self.skip_newlines();
        while !self.at(TokenKind::RBrace) {
            if self.eat(TokenKind::DotDot) {
                let name = self.expect_ident(
                    "a plain reference to spread (a reg/local/param name, not a general \
                     expression)",
                )?;
                self.skip_newlines();
                // A dedicated check, not a bare `expect(RBrace, ...)`:
                // anything trailing the identifier (`(`, `.`, `[`, a
                // comma for a second field after `..base`, ...) means
                // either `base` wasn't actually a bare reference, or
                // `..base` wasn't the LAST item (Rust's own rule) —
                // the generic "expected `}`" wouldn't say why either
                // one matters here.
                if !self.at(TokenKind::RBrace) {
                    self.error_here(
                        "`..base` must be a plain reference and the LAST item in a struct \
                         literal -- no field/another `..` may follow it, and `base` can't \
                         be a call/field access/other expression; bind it with an ordinary \
                         `let` first, then spread that"
                            .to_string(),
                    );
                    self.sync();
                    return None;
                }
                base = Some(self.ast.push_expr(Expr::Ident(name.text), name.span));
                break;
            }
            let fname = self.expect_ident("field name")?;
            self.expect(TokenKind::Colon, "`:` before field value")
                .ok()?;
            let value = self.parse_expr(0)?;
            fields.push((fname.text, value));
            self.skip_newlines();
            if !self.eat(TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        self.expect(TokenKind::RBrace, "`}`").ok()?;
        Some((fields, base))
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

/// `<width>'<radix?><value>` — e.g. `8'd6`, `8'hFF`, `8'b1010`, `8'o17`,
/// or `8'6` (no radix letter, defaulting to decimal like `'d`).
fn parse_sized_int(text: &str) -> Option<(u64, u64)> {
    let (width, rest) = text.split_once('\'')?;
    let width = width.replace('_', "").parse().ok()?;
    let rest = rest.replace('_', "");
    let value = match rest.as_bytes().first() {
        Some(b'd') => rest[1..].parse().ok()?,
        Some(b'h') => u64::from_str_radix(&rest[1..], 16).ok()?,
        Some(b'b') => u64::from_str_radix(&rest[1..], 2).ok()?,
        Some(b'o') => u64::from_str_radix(&rest[1..], 8).ok()?,
        _ => rest.parse().ok()?,
    };
    Some((width, value))
}
