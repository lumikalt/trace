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

    /// Whether this operator is one of the six comparisons — the single
    /// shared predicate types.rs/effects.rs/firrtl (checks.rs, calls.rs,
    /// writes.rs, expr.rs) all key off of for comparisons' fallible
    /// treatment (see TODO.md's "Comparisons returning their left
    /// operand" design), the same rationale `resolve::is_guard_like`
    /// documents for its own callers: independent re-derivations drift
    /// out of agreement with each other.
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        )
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
    /// `A or B or C` — Verse's failure-discharging fallback chain
    /// (`08_failure`). Left-associative but flattened into one list at
    /// parse time rather than nested `Binary`s: every consumer (effects,
    /// types, firrtl) needs "all but possibly the last are alternatives,
    /// the last may be an infallible default" as a direct index, not a
    /// left-spine walk. Always at least 2 elements.
    Or(Vec<ExprId>),
    /// `Name { field: expr, ... }` — a struct literal. `name` is the
    /// already-parsed `Ident` naming the struct type, resolved normally
    /// (like a `Call`'s own callee) rather than stored as a bare
    /// `String`, so resolve.rs's existing ident-resolution machinery
    /// finds the struct's `DefKind::Struct` def for free. Recognized in
    /// the parser's postfix loop only when `{` is immediately followed
    /// by `ident :` (not `:=`) — no statement shape starts that way, so
    /// this never misfires on an ordinary `if cond { ... }` block whose
    /// condition happens to be a bare identifier.
    StructLit {
        name: ExprId,
        fields: Vec<(String, ExprId)>,
        /// `..base` — trailing-only (Rust's own spelling), fills every
        /// field NOT named in `fields` from `base`'s own same-named
        /// field, one flat register read each (`compile_struct_field_
        /// read`, expr.rs) — never a recursive merge into a nested
        /// struct/Option field that's itself only partially given
        /// (`Pair{ inner: Inner{ x: 1 }, ..old }` does NOT reach into
        /// `inner`'s own missing fields, matching Rust's `..` exactly:
        /// it only ever fills fields absent from THIS literal's own
        /// list). `base` is restricted to a bare `Expr::Ident` — parser-
        /// enforced (`parse_struct_lit_fields`), not just documented —
        /// a general expression would need re-evaluating once per
        /// missing field, silently duplicating a call the same way an
        /// unrestricted `let {..} = source` destructuring source would.
        base: Option<ExprId>,
    },
    /// `?T` in a type position — sugar for a compiler-synthesized struct
    /// `{ valid: bit, data: T }` (`Ty::Option`, types.rs). `inner` is
    /// `T`'s own type expression. A prefix form (`?` before its operand),
    /// unlike the postfix guard `?` (`cond?`) — the two share a token but
    /// occupy disjoint parser positions (type position vs. a completed
    /// value expression), so there's no ambiguity to resolve.
    OptionTy(ExprId),
    /// `false` — the literal absent value for a `?T`-typed target. Types
    /// as the sentinel `Ty::AbsentLit`, which only unifies against a
    /// `Ty::Option` target (`check_assignable`); used anywhere else, it's
    /// a clean type error rather than a general `bits[1]` zero (see
    /// `TokenKind::False`'s own doc comment for why it stays this
    /// narrow).
    Absent,
    /// `optional <expr>` — an explicit one-layer "present" constructor,
    /// the way to build a `??T` whose two `valid` bits differ (`Some(None)`,
    /// outer present/inner absent — otherwise inexpressible, since a bare
    /// value or `false` coerces through EVERY remaining `?` layer at once).
    /// Like `Absent`, has no standalone type of its own — types as the
    /// sentinel `Ty::Optional(inner)`, which only unifies against a
    /// `Ty::Option` target, recursing on `inner` against the target's own
    /// inner rather than a type it precomputed, so nesting (`optional
    /// (optional e)`) peels one target layer per `optional`.
    Optional(ExprId),
    /// `logic <expr>` — converts a fallible expression's success into a
    /// plain `bits[1]` value (`1` if it would succeed, `0` if it would
    /// fail), discharging the failure rather than propagating it, and
    /// without performing the operand's own side effect (no real
    /// dequeue/enqueue, no callee write). A real prefix operator, not a
    /// builtin call — Verse's own `logic{ exp }` is a dedicated cast
    /// form, not an ordinary function; matches this language's `not`/
    /// `optional`/`spawn` prefix operators instead of `prio`/`trunc`/
    /// `pack`'s call syntax. `inner` must be exactly one of two shapes
    /// (a fifo op, or a call to a guard-only `<fails>` fn/impl) —
    /// checked in `firrtl/checks.rs`'s `check_logic_args`, once
    /// effects.rs's inferred signatures exist to consult, not here.
    Logic(ExprId),
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
    /// `break` — exits the enclosing `while`/`while let` loop early. v0-
    /// restricted to tail position (lower.rs's `find_break_misplaced`);
    /// fully consumed by sequences lowering (lower.rs's `render_loop_
    /// body`), never reaches firrtl.rs.
    Break,
    Return(Option<ExprId>),
    If {
        cond: ExprId,
        then_body: Vec<StmtId>,
        else_body: Option<Vec<StmtId>>,
    },
    /// `if let NAME = EXPR { then_body } [else { else_body }]` — Option-
    /// presence binding sugar (DESIGN.md's "`if let`: branch-scoped
    /// Option-presence binding"). `init` (`EXPR`) is parsed as an
    /// ordinary expression, not restricted by the parser to any
    /// particular shape — types.rs requires it be `Expr::Guard(inner)`
    /// with `inner : Ty::Option(T)` (v0: Option only, not a fifo op/
    /// failing call/comparison). `name` is bound to the unwrapped `T`
    /// value, visible ONLY within `then_body` (never `else_body`, never
    /// after the whole statement) — a genuinely new scoping rule, unlike
    /// `Stmt::Let`'s body-wide visibility.
    IfLet {
        name: Name,
        init: ExprId,
        then_body: Vec<StmtId>,
        else_body: Option<Vec<StmtId>>,
    },
    While {
        cond: ExprId,
        body: Vec<StmtId>,
    },
    /// `while let NAME = EXPR { body }` — the loop-shaped sibling of
    /// `Stmt::IfLet` (DESIGN.md's "`while`: multi-cycle loops"): each
    /// iteration re-checks `EXPR`'s presence, binding `NAME` to the
    /// unwrapped value for that iteration only — visible ONLY within
    /// `body`, never after the loop, same new-scoping-rule shape
    /// `IfLet`'s `then_body` already has. No `else` (a loop has nothing
    /// to run once instead of looping — absence just ends the loop, the
    /// same way `while`'s own condition going false does). `init`
    /// carries the identical v0 restriction `IfLet`'s does: types.rs
    /// requires `Expr::Guard(inner)` with `inner : Ty::Option(T)`, never
    /// a fifo op/failing call/comparison.
    WhileLet {
        name: Name,
        init: ExprId,
        body: Vec<StmtId>,
    },
}

/// One effect atom from an `<...>` list: `sequences`, `reads {pc, mem}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    pub name: Name,
    pub args: Vec<Name>,
}

/// `bound`/`lower` (v12): on an `Item::Fn`/`Impl` parameter, a call-site
/// obligation checked at every call (see `bounds.rs`'s module doc). On
/// `Item::Struct`'s own `fields: Vec<Param>` (v18), a flat per-field
/// value bound checked at every `StructLit` construction site and
/// (soundly, for a reg/out or a struct-typed local bound directly to a
/// literal) composed back at reads -- see `bounds.rs`'s `struct_field_
/// bounds`/`struct_field_bound`. Mirrors `Item::Reg`'s own `bound`/
/// `lower` fields in shape either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: Name,
    pub ty: ExprId,
    pub bound: Option<ExprId>,
    pub lower: Option<ExprId>,
}

/// An `extmodule` port's direction, spelled with the SAME keywords an
/// ordinary module's own ports use (`in`/`out`/`io`) — an extmodule's
/// `input`/`output` (its own FIRRTL declaration's perspective) map onto
/// exactly the same `DefKind::Input`/`Output`/`Io` an ordinary module's
/// ports already produce, so `inst.port` read/write checks (types.rs's
/// `find_port`) need no awareness of which kind of module declared the
/// port at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtPortDir {
    In,
    Out,
    Io,
}

/// One `extmodule` port declaration. Not a separately `declare`d item
/// (like `Item::Struct`'s own `fields: Vec<Param>` — plain structural
/// data on the `Item::ExtModule` node, never its own `DefId`); only the
/// `extmodule`'s own name resolves to a def, the way a struct's fields
/// don't either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtPort {
    pub dir: ExtPortDir,
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
    /// `extmodule Name from "path.v" { port... }` — declares an external
    /// Verilog module's interface; `path` is opaque data trace never
    /// reads or validates (the compiler pipeline stays pure text-in/
    /// text-out — FIRRTL emission never mentions it either, confirmed by
    /// hand-lowering an extmodule through firtool: the `.v` implementation
    /// is entirely a downstream build/simulation concern, not a FIRRTL-
    /// level one). `inst x : Name` instantiates it exactly like an
    /// ordinary module (see `Item::Inst`).
    ExtModule {
        name: Name,
        path: String,
        ports: Vec<ExtPort>,
    },
    Reg {
        name: Name,
        ty: ExprId,
        init: Option<ExprId>,
        /// `where <name> < <const>` — an optional, statically PROVEN
        /// upper bound (`bounds.rs`) on this reg's own value, checked
        /// at every write site in the program via induction, not
        /// trusted. Parsed as an ordinary expression (not restricted to
        /// any particular shape by the parser, same precedent as
        /// `IfLet`'s own `init`) — resolve.rs requires the LHS resolve
        /// to this SAME def, `bounds.rs` requires the whole shape be
        /// `Binary { Lt, Ident(self), <compile-time-constant> }`.
        bound: Option<ExprId>,
        /// `where <const> <= <name> < <const>` — the same bound's
        /// optional LOWER end (inclusive), only set when the two-sided
        /// surface form was written; `None` means the implicit floor 0
        /// (a `bits[N]` value is unsigned). Kept as a sibling field
        /// rather than folded into `bound`'s own AST shape so `bound`
        /// stays exactly `Binary { Lt, Ident(self), <const> }` either
        /// way — resolve.rs's self-reference check needs no changes for
        /// this field to exist.
        lower: Option<ExprId>,
    },
    Mem {
        name: Name,
        ty: ExprId,
        /// `where _ < <const>` — an optional, statically PROVEN bound
        /// (`bounds.rs`, v17) on every value ever WRITTEN to this mem,
        /// checked at every write site via the same per-site induction
        /// argument a reg/out's own bound already uses. Deliberately
        /// NOT handed back at read sites (a mem read still composes to
        /// `None`, exactly as before v17): an earlier version of this
        /// feature did compose at reads, but a mem has no `init`/reset,
        /// so "every WRITTEN value satisfies the bound" doesn't imply
        /// "every READ returns an in-range value" — see `bounds.rs`'s
        /// module doc for the full soundness argument an advisor pass
        /// caught before this shipped. The self-reference placeholder is
        /// `_` (`Expr::Wildcard`), not this mem's own name -- a mem
        /// element has no scoped `DefId` of its own to compare against,
        /// the same situation `ret_bound`'s own placeholder is in,
        /// unlike a reg/out/param's bound (which references a real
        /// binding already in scope). Checked by SHAPE in `resolve.rs`'s
        /// `check_mem_bound_shape`, mirroring `check_ret_bound_shape`.
        bound: Option<ExprId>,
        /// `where <const> <= _ < <const>` — the same bound's optional
        /// LOWER end, mirroring `Item::Reg`'s own `lower` field exactly.
        lower: Option<ExprId>,
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
        /// A statically PROVEN bound (`bounds.rs`), identical in shape
        /// and meaning to `Item::Reg`'s own `bound`/`lower` pair — `out`
        /// is register-backed and written via the same `Stmt::Assign`
        /// shape a `reg` is, so the same induction argument applies
        /// unchanged; see `Item::Reg`'s own doc comment for the details.
        bound: Option<ExprId>,
        lower: Option<ExprId>,
    },
    /// `io name : ty` — a structural bidirectional port (lowers to
    /// FIRRTL's `Analog` type). No initializer: it carries no value a
    /// reset could apply to. Never readable/writable from a rule body —
    /// see `Item::Attach`, its only legal use.
    Io {
        name: Name,
        ty: ExprId,
    },
    /// `attach a, b` — wires two `io` ports together (FIRRTL's own
    /// `attach`). `a`/`b` are always a bare `Ident` (a module-local `io`
    /// port) or a `Field { base: Ident(inst), name }` (an instance's `io`
    /// port); parsed as general expressions, like `Stmt::Assign`'s `lhs`,
    /// and validated to that shape in resolve.rs/types.rs rather than
    /// restricted at parse time.
    Attach {
        a: ExprId,
        b: ExprId,
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
        /// `where _ < N` / `where L <= _ < N` (v13): a postcondition on
        /// the fn's own return value, checked against every `Stmt::
        /// Return` in this fn's body (bounds.rs) and trusted at every
        /// call site so a caller can compose with the call's own
        /// result — the mirror of `Param`'s `bound`/`lower` (v12), which
        /// checks the opposite direction (an argument against the
        /// callee's declared precondition). `_` (`Expr::Wildcard`) is a
        /// shape placeholder, not a real scoped binding: unlike a
        /// reg/out/param's self-reference (an existing `DefId`), a
        /// return value is never a named binding anywhere in scope, so
        /// there's nothing to declare it against (resolve.rs).
        ret_bound: Option<ExprId>,
        ret_lower: Option<ExprId>,
        effects: Vec<Effect>,
        body: Vec<StmtId>,
    },
    Schedule {
        directives: Vec<ScheduleDirective>,
    },
    /// `struct Name { field : ty, ... }` — a declarable record type. v0:
    /// every field must be a plain `bits[N]` (no nested structs); reused
    /// `Param` for the field list since the shape (`name : ty`) is
    /// identical to a function parameter's.
    Struct {
        name: Name,
        fields: Vec<Param>,
    },
    /// `invariant <expr>;` — a module-level, statically PROVEN relational
    /// fact spanning MULTIPLE `reg`/`out` defs (`bounds.rs`), unlike
    /// `Reg`/`Output`'s own `bound`, which is a single def's own range.
    /// `expr` is parsed as an ordinary expression, no new grammar: a
    /// signed sum of reg/out idents and integer literals, optionally
    /// wrapped in `% <const>` (the declared modulus — an existing binary
    /// operator, not new syntax), compared via `<`/`<=`/`==` against a
    /// literal. `bounds.rs`'s `collect_relational_bounds` recognizes this
    /// shape and rejects anything else; resolve.rs only resolves the
    /// idents inside `expr` (ordinary variable references, no self-
    /// reference placeholder — every ident here already names a real,
    /// in-scope def, unlike `_` in a `Reg`/`Mem`/`Fn` bound).
    Invariant {
        expr: ExprId,
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
    /// One entry per `let {...} = source` destructuring pattern parsed
    /// (see `parser::parse_let_destructure`). Kept as a side list rather
    /// than a real `Stmt`/`Expr` variant: unlike `Expr::Optional` or
    /// struct update's `base`, no downstream pass (resolve/effects/
    /// elaborate/lower/firrtl) needs to know a run of `Stmt::Let`s came
    /// from a destructuring pattern — they're ordinary lets over
    /// `Expr::Field` projections either way, and forgetting to consult
    /// this list anywhere but types.rs only means a missing diagnostic,
    /// never wrong hardware.
    pub destructures: Vec<Destructure>,
    /// `Expr::Binary` ids parsed with a trailing `.!` on their operator
    /// (`a >>.! 300`) — same "side list, not a new shape" rationale as
    /// `destructures`: consumed only by types.rs's `type_binop` (to skip
    /// `check_literal_fits`/`check_shift_amount` for that one operator
    /// application), never needed by resolve/effects/elaborate/lower/
    /// firrtl, which all only ever look at `op`/`lhs`/`rhs` regardless of
    /// whether this set contains a given id.
    pub lossy: std::collections::HashSet<ExprId>,
    /// Operand `ExprId`s of an `Expr::Logic` node synthesized by `A and
    /// B`'s parse-time desugar into `(logic A) & (logic B)` (see
    /// `TokenKind::And`'s doc comment, lexer.rs) — never a `Logic` the
    /// user wrote by hand. Consulted only by `check_logic_args_in`
    /// (firrtl/checks.rs) to phrase its "not a legal operand" error in
    /// terms of `and`, not a `logic` keyword the user never typed.
    pub and_sugar: std::collections::HashSet<ExprId>,
}

/// Metadata for one `let {...} = source` destructuring pattern, consumed
/// only by types.rs's exhaustiveness check (`TypeChecker::check_
/// destructures`).
#[derive(Debug, Clone)]
pub struct Destructure {
    /// The whole `let {...} = source` statement's span, for the
    /// exhaustiveness error.
    pub span: Span,
    /// One item's synthesized `Expr::Field { base, .. }` base — already
    /// type-checked as part of the ordinary per-statement walk (each
    /// item's `Stmt::Let` types its own `init`, which recurses into
    /// `base`), so types.rs can read `source`'s type back out of
    /// `expr_tys` after the fact instead of re-typing it separately
    /// (which would need its own, easy-to-get-wrong copy of whatever
    /// `locals` snapshot was live at this exact point in the body).
    pub source_field_base: ExprId,
    /// Field names the pattern actually named (not their bind names).
    pub named_fields: Vec<Name>,
    /// Whether the pattern ended in a trailing `..`.
    pub has_rest: bool,
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
            Expr::Or(alts) => {
                let mut out = "(or".to_string();
                for alt in alts {
                    out.push(' ');
                    out.push_str(&self.expr_sexpr(*alt));
                }
                out.push(')');
                out
            }
            Expr::StructLit { name, fields, base } => {
                let mut out = format!("(struct {}", self.expr_sexpr(*name));
                for (fname, fexpr) in fields {
                    out.push_str(&format!(" ({fname} {})", self.expr_sexpr(*fexpr)));
                }
                if let Some(base) = base {
                    out.push_str(&format!(" (.. {})", self.expr_sexpr(*base)));
                }
                out.push(')');
                out
            }
            Expr::OptionTy(inner) => format!("(option {})", self.expr_sexpr(*inner)),
            Expr::Absent => "false".to_string(),
            Expr::Optional(inner) => format!("(optional {})", self.expr_sexpr(*inner)),
            Expr::Logic(inner) => format!("(logic {})", self.expr_sexpr(*inner)),
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
            Item::Reg {
                name,
                ty,
                init,
                bound,
                lower,
            } => {
                out.push_str(&format!("{pad}reg {name} : {}", self.expr_sexpr(*ty)));
                if let Some(bound) = bound {
                    match lower {
                        Some(lower) => out.push_str(&format!(
                            " where {} <= {}",
                            self.expr_sexpr(*lower),
                            self.expr_sexpr(*bound)
                        )),
                        None => out.push_str(&format!(" where {}", self.expr_sexpr(*bound))),
                    }
                }
                if let Some(init) = init {
                    out.push_str(&format!(" = {}", self.expr_sexpr(*init)));
                }
                out.push('\n');
            }
            Item::Mem {
                name,
                ty,
                bound,
                lower,
            } => {
                out.push_str(&format!("{pad}mem {name} : {}", self.expr_sexpr(*ty)));
                if let Some(bound) = bound {
                    match lower {
                        Some(lower) => out.push_str(&format!(
                            " where {} <= {}",
                            self.expr_sexpr(*lower),
                            self.expr_sexpr(*bound)
                        )),
                        None => out.push_str(&format!(" where {}", self.expr_sexpr(*bound))),
                    }
                }
                out.push('\n');
            }
            Item::Fifo { name, ty } => {
                out.push_str(&format!("{pad}fifo {name} : {}\n", self.expr_sexpr(*ty)));
            }
            Item::Input { name, ty } => {
                out.push_str(&format!("{pad}in {name} : {}\n", self.expr_sexpr(*ty)));
            }
            Item::Output {
                name,
                ty,
                init,
                bound,
                lower,
            } => {
                out.push_str(&format!("{pad}out {name} : {}", self.expr_sexpr(*ty)));
                if let Some(bound) = bound {
                    match lower {
                        Some(lower) => out.push_str(&format!(
                            " where {} <= {}",
                            self.expr_sexpr(*lower),
                            self.expr_sexpr(*bound)
                        )),
                        None => out.push_str(&format!(" where {}", self.expr_sexpr(*bound))),
                    }
                }
                if let Some(init) = init {
                    out.push_str(&format!(" = {}", self.expr_sexpr(*init)));
                }
                out.push('\n');
            }
            Item::Io { name, ty } => {
                out.push_str(&format!("{pad}io {name} : {}\n", self.expr_sexpr(*ty)));
            }
            Item::Attach { a, b } => {
                out.push_str(&format!(
                    "{pad}attach {}, {}\n",
                    self.expr_sexpr(*a),
                    self.expr_sexpr(*b)
                ));
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
                ret_bound,
                ret_lower,
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
                    .map(|p| {
                        let mut s = format!("{} : {}", p.name, self.expr_sexpr(p.ty));
                        if let Some(bound) = p.bound {
                            match p.lower {
                                Some(lower) => s.push_str(&format!(
                                    " where {} <= {}",
                                    self.expr_sexpr(lower),
                                    self.expr_sexpr(bound)
                                )),
                                None => s.push_str(&format!(" where {}", self.expr_sexpr(bound))),
                            }
                        }
                        s
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("{pad}{keyword} {name}({params})"));
                if let Some(ret) = ret {
                    out.push_str(&format!(" : {}", self.expr_sexpr(*ret)));
                }
                if let Some(ret_bound) = ret_bound {
                    match ret_lower {
                        Some(lower) => out.push_str(&format!(
                            " where {} <= {}",
                            self.expr_sexpr(*lower),
                            self.expr_sexpr(*ret_bound)
                        )),
                        None => out.push_str(&format!(" where {}", self.expr_sexpr(*ret_bound))),
                    }
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
            Item::Struct { name, fields } => {
                out.push_str(&format!("{pad}struct {name}\n"));
                let inner = "  ".repeat(depth + 1);
                for field in fields {
                    out.push_str(&format!(
                        "{inner}{} : {}\n",
                        field.name,
                        self.expr_sexpr(field.ty)
                    ));
                }
            }
            Item::Invariant { expr } => {
                out.push_str(&format!("{pad}invariant {}\n", self.expr_sexpr(*expr)));
            }
            Item::ExtModule { name, path, ports } => {
                out.push_str(&format!("{pad}extmodule {name} from {path:?}\n"));
                let inner = "  ".repeat(depth + 1);
                for port in ports {
                    let kw = match port.dir {
                        ExtPortDir::In => "in",
                        ExtPortDir::Out => "out",
                        ExtPortDir::Io => "io",
                    };
                    out.push_str(&format!(
                        "{inner}{kw} {} : {}\n",
                        port.name,
                        self.expr_sexpr(port.ty)
                    ));
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
            Stmt::Break => out.push_str(&format!("{pad}break\n")),
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
            Stmt::IfLet {
                name,
                init,
                then_body,
                else_body,
            } => {
                out.push_str(&format!(
                    "{pad}(if let {name} {})\n",
                    self.expr_sexpr(*init)
                ));
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
            Stmt::WhileLet { name, init, body } => {
                out.push_str(&format!(
                    "{pad}(while let {name} {})\n",
                    self.expr_sexpr(*init)
                ));
                for stmt in body {
                    self.dump_stmt(*stmt, depth + 1, out);
                }
            }
        }
    }
}

/// Renders an effect list the same way source syntax spells it (` <reads,
/// writes {a, b}>`, empty string if there are none) — shared between the
/// debug dump above and the language server's function-signature hover.
pub fn effects_str(effects: &[Effect]) -> String {
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

/// Stage 3's own operator generalization (v21, DESIGN.md's "Toward a
/// dependent/refinement type system"): given a `where` bound's own
/// comparison operator, which side is the self-reference, the OTHER
/// side's own folded constant value, and the def's declared bit width,
/// returns the `[lower, upper)` interval that comparison expresses.
/// `parser.rs`'s `parse_where_bound` parses positionally and doesn't
/// track which operand the user meant as self, so `_ > K` and `K < _`
/// both reach a caller as `(Gt, self_on_lhs: true)`/`(Lt, self_on_lhs:
/// false)` respectively — both mean "self must exceed K", and both
/// normalize to the SAME interval here, which is the whole point:
/// whichever side self landed on, and whichever of the four operators
/// was used, this collapses back to the one interval shape both
/// `bounds.rs` (checking writes against the declared bound) and
/// `types/stmt.rs` (checking a reg/out's own init value against it)
/// need — kept here, shared, specifically because those two call sites
/// used to duplicate the (much narrower) `_ < K`-only version of this
/// same math independently, and a where-bound with a non-`Lt` operator
/// silently reaching only ONE of the two checkers is exactly the kind
/// of two-folders-drift-apart bug this project has already shipped once
/// (see `bounds.rs`'s own `const_fold` vs `types/eval.rs`'s
/// `const_eval` history).
///
/// `None` on `limit + 1` overflowing `u64` (only reachable at `limit ==
/// u64::MAX`, already wider than any real `bits[N]` this compiler
/// supports) or `1 << width` overflowing (`width >= 64`, same
/// reasoning) — neither is a case worth its own diagnostic.
pub fn normalize_where_relation(
    op: BinOp,
    self_on_lhs: bool,
    limit: u64,
    width: u64,
) -> Option<(u64, u64)> {
    let ceiling = 1u64.checked_shl(width as u32)?;
    match (op, self_on_lhs) {
        (BinOp::Lt, true) | (BinOp::Gt, false) => Some((0, limit)),
        (BinOp::Le, true) | (BinOp::Ge, false) => limit.checked_add(1).map(|u| (0, u)),
        (BinOp::Gt, true) | (BinOp::Lt, false) => limit.checked_add(1).map(|l| (l, ceiling)),
        (BinOp::Ge, true) | (BinOp::Le, false) => Some((limit, ceiling)),
        _ => None,
    }
}
