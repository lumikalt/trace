//! Index-based AST: nodes live in per-kind arenas, children are typed ids.
//! Spans sit in parallel vectors. Analysis passes attach side tables keyed
//! by id instead of mutating nodes.

use crate::lexer::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExprId(pub u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StmtId(pub u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(pub u32);

/// An identifier together with its source span. Everything the resolver
/// can report an error against carries one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name {
    pub text: String,
    pub span: Span,
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    BitNot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Shl,
    Shr,
    /// Arithmetic right shift (`>>>`): sign-extends the vacated high bits
    /// instead of `Shr`'s zero-fill — the interpretation is a per-
    /// operator choice, not a property of a signed type, since this
    /// language has no signed type (see TODO.md).
    AShr,
    BitAnd,
    BitOr,
    BitXor,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Range,
    /// Verilog-style indexed part-select, ascending: `x[base +: width]` —
    /// only meaningful as a `Bracket`'s own argument, same restriction as
    /// `Range`.
    PlusColon,
    /// The descending mirror of `PlusColon`: `x[base -: width]`.
    MinusColon,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::AShr => ">>>",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
            BinOp::Eq => "=",
            BinOp::Ne => "<>",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::Range => "..",
            BinOp::PlusColon => "+:",
            BinOp::MinusColon => "-:",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Ident(String),
    Int(u64),
    /// A Verilog-style sized literal, `<width>'<radix?><value>` (e.g.
    /// `8'd6`, `8'hFF`, `8'6`) — unlike `Int`, which has no width of its
    /// own and absorbs one from context, this types directly as
    /// `Ty::Bits(Width::Known(width))` (types.rs), checked for overflow
    /// against ITS OWN declared width immediately, not deferred to a
    /// later coercion site.
    SizedInt {
        width: u64,
        value: u64,
    },
    Wildcard,
    Unary {
        op: UnOp,
        operand: ExprId,
    },
    Binary {
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
    },
    /// `e?` — guard; failure aborts the enclosing rule for this cycle.
    Guard(ExprId),
    Field {
        base: ExprId,
        name: String,
    },
    /// `f(a, b)` — infallible application.
    Call {
        callee: ExprId,
        args: Vec<ExprId>,
    },
    /// `f[a]` — bracket application: fallible call, memory index, or type
    /// parameter (`bits[8]`). Semantic passes tell them apart.
    Bracket {
        callee: ExprId,
        args: Vec<ExprId>,
    },
    /// `spawn e` — start a parallel FSM.
    Spawn(ExprId),
    /// `[e1, e2, ...]` — a `list[T]` literal; elaboration-time only (its
    /// LENGTH is a compile-time fact, not a circuit value), each element
    /// an ordinary `<combines>`-valued expression.
    ListLit(Vec<ExprId>),
    /// `..hi`, `lo..`, or `lo..hi` used as ONE bracket argument — a
    /// list slice bound (`xs[..mid]`/`xs[mid..]`). Distinct from the
    /// existing two-sided `BinOp::Range` (`Expr::Binary`), which stays
    /// the required-both-sides bit-slice `x[hi..lo]`; this variant only
    /// exists when at least one side is OMITTED, exclusively for
    /// elaboration-time list slicing. `lo`/`hi` are never both `None`
    /// (parser only constructs this when at least one side is absent)
    /// nor both `Some` (that's the existing `Expr::Binary` shape).
    Range {
        lo: Option<ExprId>,
        hi: Option<ExprId>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    Expr(ExprId),
    /// `lhs := rhs` — transactional write, visible at the cycle boundary.
    Assign {
        lhs: ExprId,
        rhs: ExprId,
    },
    Let {
        name: Name,
        init: ExprId,
    },
    Tick,
    Return(Option<ExprId>),
    If {
        cond: ExprId,
        then_body: Vec<StmtId>,
        else_body: Option<Vec<StmtId>>,
    },
    While {
        cond: ExprId,
        body: Vec<StmtId>,
    },
}

/// One effect atom from an `<...>` list: `sequences`, `reads {pc, mem}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    pub name: Name,
    pub args: Vec<Name>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: Name,
    pub ty: ExprId,
}

/// `fn` vs `spec` vs `impl ... refines Spec`. Specs may declare `chooses`;
/// impls are checked as refinements of the spec they name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FnKind {
    Fn,
    Spec,
    Impl { refines: Name },
}

/// One directive in a `schedule { ... }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleDirective {
    /// `urgency a > b > c` — descending priority.
    Urgency(Vec<Name>),
    /// `mutually_exclusive { a, b }` — claims the two rules never both
    /// fire the same cycle; checked with a simulation assertion. Named
    /// to match Bluespec's own vocabulary (this project's cited
    /// scheduling reference), where `conflict_free` means something
    /// different — see `ConflictFree` below.
    MutuallyExclusive(Vec<Name>),
    /// `conflict_free { a, b }` — claims it's safe for both to fire the
    /// same cycle (e.g. genuinely separate ports on one resource).
    /// Trusted, NOT checked: v0 has no way to prove or check address
    /// disjointness (that's the tier-3 banked-array proof DESIGN.md
    /// defers), so unlike `MutuallyExclusive` there is no assertion to
    /// insert here — only the derived stall is waived.
    ConflictFree(Vec<Name>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Module {
        name: Name,
        items: Vec<ItemId>,
    },
    Reg {
        name: Name,
        ty: ExprId,
        init: Option<ExprId>,
    },
    Mem {
        name: Name,
        ty: ExprId,
    },
    Fifo {
        name: Name,
        ty: ExprId,
    },
    /// `in name : ty` — external combinational signal, read-only.
    Input {
        name: Name,
        ty: ExprId,
    },
    /// `out name : ty (= init)?` — register-backed, exposed as a port.
    Output {
        name: Name,
        ty: ExprId,
        init: Option<ExprId>,
    },
    /// `inst name : Module` — a child module instance. `module` is an
    /// identifier expression naming a sibling top-level `module`, not a
    /// `bits[...]` type; ports are accessed as `name.port`.
    Inst {
        name: Name,
        module: ExprId,
    },
    Rule {
        name: Name,
        effects: Vec<Effect>,
        body: Vec<StmtId>,
    },
    Fn {
        name: Name,
        kind: FnKind,
        params: Vec<Param>,
        ret: Option<ExprId>,
        effects: Vec<Effect>,
        body: Vec<StmtId>,
    },
    Schedule {
        directives: Vec<ScheduleDirective>,
    },
}

#[derive(Debug, Default)]
pub struct Ast {
    pub exprs: Vec<Expr>,
    pub expr_spans: Vec<Span>,
    pub stmts: Vec<Stmt>,
    pub stmt_spans: Vec<Span>,
    pub items: Vec<Item>,
    pub item_spans: Vec<Span>,
    /// Top-level items in source order.
    pub roots: Vec<ItemId>,
}

impl Ast {
    pub fn expr(&self, id: ExprId) -> &Expr {
        &self.exprs[id.0 as usize]
    }

    pub fn stmt(&self, id: StmtId) -> &Stmt {
        &self.stmts[id.0 as usize]
    }

    pub fn item(&self, id: ItemId) -> &Item {
        &self.items[id.0 as usize]
    }

    pub fn push_expr(&mut self, expr: Expr, span: Span) -> ExprId {
        self.exprs.push(expr);
        self.expr_spans.push(span);
        ExprId(self.exprs.len() as u32 - 1)
    }

    pub fn push_stmt(&mut self, stmt: Stmt, span: Span) -> StmtId {
        self.stmts.push(stmt);
        self.stmt_spans.push(span);
        StmtId(self.stmts.len() as u32 - 1)
    }

    pub fn push_item(&mut self, item: Item, span: Span) -> ItemId {
        self.items.push(item);
        self.item_spans.push(span);
        ItemId(self.items.len() as u32 - 1)
    }

    /// Render an expression as an s-expression: `(+ a (* b c))`.
    /// Tests assert on this; the CLI dumps it.
    pub fn expr_sexpr(&self, id: ExprId) -> String {
        match self.expr(id) {
            Expr::Ident(name) => name.clone(),
            Expr::Int(value) => value.to_string(),
            Expr::SizedInt { width, value } => format!("{width}'d{value}"),
            Expr::Wildcard => "_".to_string(),
            Expr::Unary { op, operand } => {
                let sym = match op {
                    UnOp::Neg => "-",
                    UnOp::Not => "not",
                    UnOp::BitNot => "~",
                };
                format!("({sym} {})", self.expr_sexpr(*operand))
            }
            Expr::Binary { op, lhs, rhs } => format!(
                "({} {} {})",
                op.symbol(),
                self.expr_sexpr(*lhs),
                self.expr_sexpr(*rhs)
            ),
            Expr::Guard(inner) => format!("(? {})", self.expr_sexpr(*inner)),
            Expr::Field { base, name } => format!("(. {} {name})", self.expr_sexpr(*base)),
            Expr::Call { callee, args } => self.app_sexpr("call", *callee, args),
            Expr::Bracket { callee, args } => self.app_sexpr("index", *callee, args),
            Expr::Spawn(inner) => format!("(spawn {})", self.expr_sexpr(*inner)),
            Expr::ListLit(items) => {
                let mut out = "(list".to_string();
                for item in items {
                    out.push(' ');
                    out.push_str(&self.expr_sexpr(*item));
                }
                out.push(')');
                out
            }
            Expr::Range { lo, hi } => format!(
                "(.. {} {})",
                lo.map(|e| self.expr_sexpr(e)).unwrap_or_default(),
                hi.map(|e| self.expr_sexpr(e)).unwrap_or_default(),
            ),
        }
    }

    fn app_sexpr(&self, tag: &str, callee: ExprId, args: &[ExprId]) -> String {
        let mut out = format!("({tag} {}", self.expr_sexpr(callee));
        for arg in args {
            out.push(' ');
            out.push_str(&self.expr_sexpr(*arg));
        }
        out.push(')');
        out
    }

    /// Render the whole file as an indented outline, for CLI debugging.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for root in &self.roots {
            self.dump_item(*root, 0, &mut out);
        }
        out
    }

    fn dump_item(&self, id: ItemId, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        match self.item(id) {
            Item::Module { name, items } => {
                out.push_str(&format!("{pad}module {name}\n"));
                for item in items {
                    self.dump_item(*item, depth + 1, out);
                }
            }
            Item::Reg { name, ty, init } => {
                out.push_str(&format!("{pad}reg {name} : {}", self.expr_sexpr(*ty)));
                if let Some(init) = init {
                    out.push_str(&format!(" = {}", self.expr_sexpr(*init)));
                }
                out.push('\n');
            }
            Item::Mem { name, ty } => {
                out.push_str(&format!("{pad}mem {name} : {}\n", self.expr_sexpr(*ty)));
            }
            Item::Fifo { name, ty } => {
                out.push_str(&format!("{pad}fifo {name} : {}\n", self.expr_sexpr(*ty)));
            }
            Item::Input { name, ty } => {
                out.push_str(&format!("{pad}in {name} : {}\n", self.expr_sexpr(*ty)));
            }
            Item::Output { name, ty, init } => {
                out.push_str(&format!("{pad}out {name} : {}", self.expr_sexpr(*ty)));
                if let Some(init) = init {
                    out.push_str(&format!(" = {}", self.expr_sexpr(*init)));
                }
                out.push('\n');
            }
            Item::Inst { name, module } => {
                out.push_str(&format!(
                    "{pad}inst {name} : {}\n",
                    self.expr_sexpr(*module)
                ));
            }
            Item::Rule {
                name,
                effects,
                body,
            } => {
                out.push_str(&format!("{pad}rule {name}{}\n", effects_str(effects)));
                for stmt in body {
                    self.dump_stmt(*stmt, depth + 1, out);
                }
            }
            Item::Fn {
                name,
                kind,
                params,
                ret,
                effects,
                body,
            } => {
                let keyword = match kind {
                    FnKind::Fn => "fn",
                    FnKind::Spec => "spec",
                    FnKind::Impl { .. } => "impl",
                };
                let params = params
                    .iter()
                    .map(|p| format!("{} : {}", p.name, self.expr_sexpr(p.ty)))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("{pad}{keyword} {name}({params})"));
                if let Some(ret) = ret {
                    out.push_str(&format!(" : {}", self.expr_sexpr(*ret)));
                }
                out.push_str(&effects_str(effects));
                if let FnKind::Impl { refines } = kind {
                    out.push_str(&format!(" refines {refines}"));
                }
                out.push('\n');
                for stmt in body {
                    self.dump_stmt(*stmt, depth + 1, out);
                }
            }
            Item::Schedule { directives } => {
                out.push_str(&format!("{pad}schedule\n"));
                let inner = "  ".repeat(depth + 1);
                for directive in directives {
                    let (tag, names) = match directive {
                        ScheduleDirective::Urgency(names) => ("urgency", names),
                        ScheduleDirective::MutuallyExclusive(names) => {
                            ("mutually_exclusive", names)
                        }
                        ScheduleDirective::ConflictFree(names) => ("conflict_free", names),
                    };
                    let names = names.iter().map(|n| n.text.as_str()).collect::<Vec<_>>();
                    out.push_str(&format!("{inner}({tag} {})\n", names.join(" ")));
                }
            }
        }
    }

    fn dump_stmt(&self, id: StmtId, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        match self.stmt(id) {
            Stmt::Expr(expr) => out.push_str(&format!("{pad}{}\n", self.expr_sexpr(*expr))),
            Stmt::Assign { lhs, rhs } => out.push_str(&format!(
                "{pad}(:= {} {})\n",
                self.expr_sexpr(*lhs),
                self.expr_sexpr(*rhs)
            )),
            Stmt::Let { name, init } => {
                out.push_str(&format!("{pad}(let {name} {})\n", self.expr_sexpr(*init)));
            }
            Stmt::Tick => out.push_str(&format!("{pad}tick\n")),
            Stmt::Return(expr) => match expr {
                Some(expr) => {
                    out.push_str(&format!("{pad}(return {})\n", self.expr_sexpr(*expr)));
                }
                None => out.push_str(&format!("{pad}(return)\n")),
            },
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                out.push_str(&format!("{pad}if {}\n", self.expr_sexpr(*cond)));
                for stmt in then_body {
                    self.dump_stmt(*stmt, depth + 1, out);
                }
                if let Some(else_body) = else_body {
                    out.push_str(&format!("{pad}else\n"));
                    for stmt in else_body {
                        self.dump_stmt(*stmt, depth + 1, out);
                    }
                }
            }
            Stmt::While { cond, body } => {
                out.push_str(&format!("{pad}while {}\n", self.expr_sexpr(*cond)));
                for stmt in body {
                    self.dump_stmt(*stmt, depth + 1, out);
                }
            }
        }
    }
}

fn effects_str(effects: &[Effect]) -> String {
    if effects.is_empty() {
        return String::new();
    }
    let inner = effects
        .iter()
        .map(|e| {
            if e.args.is_empty() {
                e.name.text.clone()
            } else {
                let args = e.args.iter().map(|a| a.text.as_str()).collect::<Vec<_>>();
                format!("{} {{{}}}", e.name, args.join(", "))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(" <{inner}>")
}
